use crate::{Command, CommandList, DataType, Error, Result, Value};
use futures::{future::BoxFuture, lock::Mutex, FutureExt};

#[cfg(feature = "runtime_async_std")]
use async_std::{
    io,
    net::{TcpStream, ToSocketAddrs},
};
#[cfg(feature = "runtime_async_std")]
use futures::{AsyncReadExt, AsyncWriteExt};

#[cfg(feature = "runtime_tokio")]
use tokio::{
    io::{self, AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, ToSocketAddrs},
};

use std::sync::Arc;

pub mod builder;
pub mod scan;
pub mod stream;
pub use scan::{HScanBuilder, HScanStream, ScanBuilder, ScanStream};
pub use stream::{Message, MessageStream, PMessage, PMessageStream, ResponseStream};

use builder::MSetBuilder;

#[cfg(test)]
mod test;

macro_rules! check_slice_not_empty {
    ($slice:ident) => {
        if $slice.is_empty() {
            return Err(Error::EmptySlice);
        }
    };
}

async fn read_until(r: &mut TcpStream, byte: u8) -> io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    let mut single = [0; 1];
    loop {
        r.read(&mut single).await?;
        buffer.push(single[0]);
        if single[0] == byte {
            return Ok(buffer);
        }
    }
}

///A connection to Redis. Copying is cheap as the inner type is a simple, futures-aware, `Arc<Mutex>`, and will
///not create a new connection. Use a [`ConnectionPool`](struct.ConnectionPool.html) if you want to use pooled connections.
///Alternatively, there's the `deadpool-darkredis` crate.
///Every convenience function can work with any kind of data as long as it can be converted into bytes.
///Check the [Redis command reference](https://redis.io/commands) for in-depth explanations of each command.
#[derive(Clone, Debug)]
pub struct Connection {
    pub(crate) stream: Arc<Mutex<TcpStream>>,
}

impl Connection {
    ///Connect to a Redis instance running at `address`. If you wish to name this connection, run the [`CLIENT SETNAME`](https://redis.io/commands/client-setname) command.
    pub async fn connect<A>(address: A) -> Result<Self>
    where
        A: ToSocketAddrs,
    {
        let stream = Arc::new(Mutex::new(
            TcpStream::connect(address)
                .await
                .map_err(Error::ConnectionFailed)?,
        ));

        Ok(Self { stream })
    }

    ///Connect to a Redis instance running at `address`, and authenticate using `password`.
    pub async fn connect_and_auth<A, P>(address: A, password: P) -> Result<Self>
    where
        A: ToSocketAddrs,
        P: AsRef<[u8]>,
    {
        let mut out = Self::connect(address).await?;
        out.run_command(Command::new("AUTH").arg(&password)).await?;

        Ok(out)
    }

    async fn parse_simple_value(buf: &[u8]) -> Result<Value> {
        match buf[0] {
            b'+' => {
                if buf == b"+OK\r\n" {
                    Ok(Value::Ok)
                } else {
                    Ok(Value::String(buf[1..].into()))
                }
            }
            b'-' => Err(Error::RedisError(
                String::from_utf8_lossy(&buf[1..]).to_string(),
            )),
            b':' => {
                let string = String::from_utf8_lossy(&buf[1..]);
                let num = string.trim().parse::<isize>().unwrap();
                Ok(Value::Integer(num))
            }
            _ => Err(Error::UnexpectedResponse(
                String::from_utf8_lossy(buf).to_string(),
            )),
        }
    }

    async fn parse_string(start: &[u8], stream: &mut TcpStream) -> Result<Value> {
        if start == b"$-1\r\n" {
            Ok(Value::Nil)
        } else {
            let num = String::from_utf8_lossy(&start[1..])
                .trim()
                .parse::<usize>()
                .unwrap();
            let mut buf = vec![0u8; num + 2]; // add two to catch the final \r\n from Redis
            stream.read_exact(&mut buf).await?;

            buf.pop(); //Discard the last \r\n
            buf.pop();
            Ok(Value::String(buf))
        }
    }

    fn parse_array<'a>(start: &'a [u8], stream: &'a mut TcpStream) -> BoxFuture<'a, Result<Value>> {
        async move {
            let num_parsed = String::from_utf8_lossy(&start[1..])
                .trim()
                .parse::<i32>()
                .unwrap();

            // result can be negative (blpop/brpop return '-1' on timeout)
            if num_parsed < 0 {
                return Ok(Value::Nil);
            }

            let num = num_parsed as usize;
            let mut values = Vec::with_capacity(num);

            for _ in 0..num {
                let buf = read_until(stream, b'\n').await?;
                match buf[0] {
                    b'+' | b'-' | b':' => values.push(Self::parse_simple_value(&buf).await?),
                    b'$' => values.push(Self::parse_string(&buf, stream).await?),
                    b'*' => values.push(Self::parse_array(&buf, stream).await?),
                    _ => {
                        return Err(Error::UnexpectedResponse(
                            String::from_utf8_lossy(&buf).to_string(),
                        ))
                    }
                }
            }

            Ok(Value::Array(values))
        }
        .boxed()
    }

    //Read a value from the connection.
    pub(crate) async fn read_value(mut stream: &mut TcpStream) -> Result<Value> {
        let buf = read_until(&mut stream, b'\n').await?;
        match buf[0] {
            b'+' | b'-' | b':' => Self::parse_simple_value(&buf).await,
            b'$' => Self::parse_string(&buf, &mut stream).await,
            b'*' => Self::parse_array(&buf, &mut stream).await,
            _ => Err(Error::UnexpectedResponse(
                String::from_utf8_lossy(&buf).to_string(),
            )),
        }
    }

    ///Run a single command on this connection.
    #[inline]
    pub async fn run_command(&mut self, command: Command<'_>) -> Result<Value> {
        let mut buffer = Vec::new();

        self.run_command_with_buffer(command, &mut buffer).await
    }

    ///Run a single command on this connection, using `buffer` for serialization.
    ///See [`run_commands_with_buffer`](struct.Connection.html#method.run_commands_with_buffer) for more details.
    pub async fn run_command_with_buffer(
        &mut self,
        command: Command<'_>,
        buffer: &mut Vec<u8>,
    ) -> Result<Value> {
        let mut stream = self.stream.lock().await;
        command.serialize(buffer);
        stream.write_all(&buffer).await?;

        Ok(Self::read_value(&mut stream).await?)
    }

    ///Run a series of commands on this connection, returning a stream of the results.
    #[inline]
    pub async fn run_commands(&mut self, command: CommandList<'_>) -> Result<ResponseStream> {
        let mut buffer = Vec::new();
        self.run_commands_with_buffer(command, &mut buffer).await
    }

    ///Run a series of commands on this connection, using `buffer` for serialization.
    ///This prevents allocations as long as `buffer` is large enough from before. The
    ///buffer will be empty when this function returns.
    pub async fn run_commands_with_buffer(
        &mut self,
        command: CommandList<'_>,
        buf: &mut Vec<u8>,
    ) -> Result<ResponseStream> {
        buf.clear();
        let mut lock = self.stream.lock().await;
        let command_count = command.command_count();
        command.serialize(buf);
        lock.write_all(&buf).await?;
        buf.clear();

        Ok(ResponseStream::new(command_count, self.stream.clone()))
    }

    ///Delete `field` from the hash set stored at `key`.
    ///# Return value
    ///`true` when the field was deleted, `false` if it didn't exist
    pub async fn hdel<K, F>(&mut self, key: K, field: F) -> Result<bool>
    where
        K: AsRef<[u8]>,
        F: AsRef<[u8]>,
    {
        self.run_command(Command::new("HDEL").arg(&key).arg(&field))
            .await
            .map(|v| v.unwrap_bool())
    }

    ///Delete every field in `fields` from the hash set stored at `key`.
    ///# Return value
    ///The number of deleted fields.
    pub async fn hdel_slice<K, F>(&mut self, key: K, fields: &[F]) -> Result<isize>
    where
        K: AsRef<[u8]>,
        F: AsRef<[u8]>,
    {
        check_slice_not_empty!(fields);

        self.run_command(Command::new("HDEL").arg(&key).args(&fields))
            .await
            .map(|v| v.unwrap_integer())
    }

    ///Check if `field` exists in the hash set `key`.
    pub async fn hexists<K, F>(&mut self, key: K, field: F) -> Result<bool>
    where
        K: AsRef<[u8]>,
        F: AsRef<[u8]>,
    {
        self.run_command(Command::new("HEXISTS").arg(&key).arg(&field))
            .await
            .map(|v| v.unwrap_bool())
    }

    ///Get the value of `field` in the hash set at `key`.
    pub async fn hget<K, F>(&mut self, key: K, field: F) -> Result<Option<Vec<u8>>>
    where
        K: AsRef<[u8]>,
        F: AsRef<[u8]>,
    {
        self.run_command(Command::new("HGET").arg(&key).arg(&field))
            .await
            .map(|v| v.optional_string())
    }

    ///Set the value of `field` in the hash set stored at `key` to `Value`.
    ///# Return value
    ///The number of added fields(will be 1 if `field` was created, 0 if it already existed).
    pub async fn hset<K, F, V>(&mut self, key: K, field: F, value: V) -> Result<isize>
    where
        K: AsRef<[u8]>,
        F: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        self.run_command(Command::new("HSET").arg(&key).arg(&field).arg(&value))
            .await
            .map(|v| v.unwrap_integer())
    }

    ///Set the value of `field` in the hash set stored at `key` to `Value`. If `field`
    ///already exists, do nothing.
    ///# Return value
    ///`true` if `field` was set, `false` otherwise.
    pub async fn hsetnx<K, F, V>(&mut self, key: K, field: F, value: V) -> Result<bool>
    where
        K: AsRef<[u8]>,
        F: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        self.run_command(Command::new("HSETNX").arg(&key).arg(&field).arg(&value))
            .await
            .map(|v| v.unwrap_bool())
    }

    ///Set fields in the hash set at `key` to their values in `builder`. See
    ///[`Connection::mset`](struct.Connection.html#method.mset) for more information.
    ///# Return value
    ///The number of added fields.
    pub async fn hset_many<K>(&mut self, key: K, builder: MSetBuilder<'_>) -> Result<isize>
    where
        K: AsRef<[u8]>,
    {
        let mut command = Command::new("HSET").arg(&key);
        command.append_msetbuilder(&builder);

        self.run_command(command).await.map(|v| v.unwrap_integer())
    }

    ///Increment `field` in the hash set `key` by `val`.
    ///# Return value
    ///The field value after the increment.
    pub async fn hincrby<K, F>(&mut self, key: K, field: F, val: isize) -> Result<isize>
    where
        K: AsRef<[u8]>,
        F: AsRef<[u8]>,
    {
        let val = val.to_string();
        self.run_command(Command::new("HINCRBY").arg(&key).arg(&field).arg(&val))
            .await
            .map(|v| v.unwrap_integer())
    }

    ///Increment `field` in the hash set `key` by `val`, floating point version.
    ///# Return value
    ///The field value after the increment.
    pub async fn hincrbyfloat<K, F>(&mut self, key: K, field: F, val: f64) -> Result<f64>
    where
        K: AsRef<[u8]>,
        F: AsRef<[u8]>,
    {
        let val = val.to_string();
        let command = Command::new("HINCRBYFLOAT").arg(&key).arg(&field).arg(&val);
        let result = self.run_command(command).await?.unwrap_string();
        Ok(String::from_utf8_lossy(&result).parse::<f64>().unwrap())
    }

    ///Get the name of each hash field stored at `key`.
    pub async fn hkeys<K>(&mut self, key: K) -> Result<Vec<Vec<u8>>>
    where
        K: AsRef<[u8]>,
    {
        self.run_command(Command::new("HKEYS").arg(&key))
            .await
            .map(|v| v.unwrap_string_array())
    }

    ///Get the number of fields in the hash stored at `key`.
    pub async fn hlen<K>(&mut self, key: K) -> Result<isize>
    where
        K: AsRef<[u8]>,
    {
        self.run_command(Command::new("HLEN").arg(&key))
            .await
            .map(|v| v.unwrap_integer())
    }

    ///Get the number of bytes in `field` in the hash set `key`
    pub async fn hstrlen<K, F>(&mut self, key: K, field: F) -> Result<isize>
    where
        K: AsRef<[u8]>,
        F: AsRef<[u8]>,
    {
        self.run_command(Command::new("HSTRLEN").arg(&key).arg(&field))
            .await
            .map(|v| v.unwrap_integer())
    }

    ///Get the value of each field in the hash field stored at `key`.
    pub async fn hvals<K>(&mut self, key: K) -> Result<Vec<Value>>
    where
        K: AsRef<[u8]>,
    {
        self.run_command(Command::new("HVALS").arg(&key))
            .await
            .map(|v| v.unwrap_array())
    }

    ///Send a `PING` to the server, returning Ok(()) on success.
    pub async fn ping(&mut self) -> Result<()> {
        self.run_command(Command::new("PING")).await.map(|_| ())
    }

    ///Consume `self`, and subscribe to `channels`, returning a stream of [`Message's`](struct.Message.html). As of now, there's no way to get the connection back, nor change the subscribed topics.
    pub async fn subscribe<K>(mut self, channels: &[K]) -> Result<stream::MessageStream>
    where
        K: AsRef<[u8]>,
    {
        let command = Command::new("SUBSCRIBE").args(channels);

        //TODO: Find out if we care about the values given here
        let _ = self.run_command(command).await?;
        {
            let mut stream = self.stream.lock().await;
            for _ in 0..channels.len() - 1 {
                let response = Self::read_value(&mut stream).await?;
                assert_eq!(
                    response.unwrap_array()[0],
                    Value::String("subscribe".into())
                );
            }
        }

        Ok(stream::MessageStream::new(self))
    }

    ///Exactly like [`subscribe`](struct.Connection.html#method.subscribe), but subscribe to channels
    ///matching patterns instead.
    pub async fn psubscribe<K>(mut self, patterns: &[K]) -> Result<stream::PMessageStream>
    where
        K: AsRef<[u8]>,
    {
        let command = Command::new("PSUBSCRIBE").args(patterns);

        //TODO: Find out if we care about the values given here
        let _ = self.run_command(command).await?;
        {
            let mut stream = self.stream.lock().await;
            for _ in 0..patterns.len() - 1 {
                let response = Self::read_value(&mut stream).await?;
                assert_eq!(
                    response.unwrap_array()[0],
                    Value::String("psubscribe".into())
                );
            }
        }

        Ok(stream::PMessageStream::new(self))
    }

    ///Publish `message` to `channel`.
    ///# Return Value
    ///Returns how many clients received the message.
    pub async fn publish<C, M>(&mut self, channel: C, message: M) -> Result<isize>
    where
        C: AsRef<[u8]>,
        M: AsRef<[u8]>,
    {
        let command = Command::new("PUBLISH").arg(&channel).arg(&message);
        self.run_command(command).await.map(|i| i.unwrap_integer())
    }

    ///Set `key` to `value`.
    pub async fn set<K, D>(&mut self, key: K, value: D) -> Result<()>
    where
        K: AsRef<[u8]>,
        D: AsRef<[u8]>,
    {
        let command = Command::new("SET").arg(&key).arg(&value);

        self.run_command(command).await.map(|_| ())
    }

    ///Set the key `key` to `data`, and set it to expire after `seconds` seconds.
    pub async fn set_and_expire_seconds<K, D>(
        &mut self,
        key: K,
        data: D,
        seconds: u32,
    ) -> Result<()>
    where
        K: AsRef<[u8]>,
        D: AsRef<[u8]>,
    {
        let seconds = seconds.to_string();
        let command = Command::new("SET")
            .arg(&key)
            .arg(&data)
            .arg(b"EX")
            .arg(&seconds);

        self.run_command(command).await.map(|_| ())
    }

    ///Set the key `key` to `data`, and set it to expire after `milliseconds` ms.
    pub async fn set_and_expire_ms<K, D>(
        &mut self,
        key: K,
        data: D,
        milliseconds: u32,
    ) -> Result<()>
    where
        K: AsRef<[u8]>,
        D: AsRef<[u8]>,
    {
        let milliseconds = milliseconds.to_string();
        let command = Command::new("SET")
            .arg(&key)
            .arg(&data)
            .arg(b"PX")
            .arg(&milliseconds);

        self.run_command(command).await.map(|_| ())
    }

    ///Set `key` to expire `seconds` seconds from now.
    pub async fn expire_seconds<K>(&mut self, key: K, seconds: u32) -> Result<isize>
    where
        K: AsRef<[u8]>,
    {
        let seconds = seconds.to_string();
        let command = Command::new("EXPIRE").arg(&key).arg(&seconds);

        self.run_command(command).await.map(|i| i.unwrap_integer())
    }

    ///Set `key` to expire `milliseconds` ms from now.
    pub async fn expire_ms<K>(&mut self, key: K, seconds: u32) -> Result<isize>
    where
        K: AsRef<[u8]>,
    {
        let seconds = seconds.to_string();
        let command = Command::new("PEXPIRE").arg(&key).arg(&seconds);

        self.run_command(command).await.map(|i| i.unwrap_integer())
    }

    ///Set `key` to expire at Unix timestamp `timestamp`, measured in seconds.
    pub async fn expire_at_seconds<K>(&mut self, key: K, timestamp: u64) -> Result<isize>
    where
        K: AsRef<[u8]>,
    {
        let timestamp = timestamp.to_string();
        let command = Command::new("EXPIREAT").arg(&key).arg(&timestamp);

        self.run_command(command).await.map(|i| i.unwrap_integer())
    }

    ///Set `key` to expire at Unix timestamp `timestamp`, measured in milliseconds.
    pub async fn expire_at_ms<K>(&mut self, key: K, timestamp: u64) -> Result<isize>
    where
        K: AsRef<[u8]>,
    {
        let timestamp = timestamp.to_string();
        let command = Command::new("PEXPIREAT").arg(&key).arg(&timestamp);

        self.run_command(command).await.map(|i| i.unwrap_integer())
    }

    ///Delete `key`.
    ///# Return value
    ///The number of deleted keys.
    pub async fn del<K>(&mut self, key: K) -> Result<bool>
    where
        K: AsRef<[u8]>,
    {
        let command = Command::new("DEL").arg(&key);
        self.run_command(command).await.map(|i| i.unwrap_bool())
    }

    ///Delete every element in `keys`.
    ///# Return value
    ///The number of deleted keys.
    pub async fn del_slice<K>(&mut self, keys: &[K]) -> Result<isize>
    where
        K: AsRef<[u8]>,
    {
        check_slice_not_empty!(keys);
        let command = Command::new("DEL").args(&keys);
        self.run_command(command).await.map(|i| i.unwrap_integer())
    }

    ///Get the value of `key`.
    pub async fn get<K>(&mut self, key: K) -> Result<Option<Vec<u8>>>
    where
        K: AsRef<[u8]>,
    {
        let command = Command::new("GET").arg(&key);

        Ok(self.run_command(command).await?.optional_string())
    }

    ///Push a value to `list` from the left.
    ///# Return value
    ///The number of elements in `list`
    pub async fn lpush<K, V>(&mut self, list: K, value: V) -> Result<isize>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        let command = Command::new("LPUSH").arg(&list).arg(&value);

        Ok(self.run_command(command).await?.unwrap_integer())
    }

    ///Like [`lpush`](struct.Connection.html#method.lpush), but push multiple values.
    pub async fn lpush_slice<K, V>(&mut self, key: K, values: &[V]) -> Result<isize>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        check_slice_not_empty!(values);
        let command = Command::new("LPUSH").arg(&key).args(values);

        Ok(self.run_command(command).await?.unwrap_integer())
    }

    ///Push a value to `list` from the right.
    ///# Return value
    ///The number of elements in `list`
    pub async fn rpush<K, V>(&mut self, list: K, value: V) -> Result<isize>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        let command = Command::new("RPUSH").arg(&list).arg(&value);

        Ok(self.run_command(command).await?.unwrap_integer())
    }

    ///Like [`rpush`](struct.Connection.html#method.rpush), but push multiple values through a slice.
    pub async fn rpush_slice<K, V>(&mut self, key: K, values: &[V]) -> Result<isize>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        check_slice_not_empty!(values);
        let command = Command::new("RPUSH").arg(&key).args(values);

        Ok(self.run_command(command).await?.unwrap_integer())
    }

    ///Pop a value from a list from the left side.
    ///# Return value
    ///The value popped from `list`
    pub async fn lpop<K>(&mut self, list: K) -> Result<Option<Vec<u8>>>
    where
        K: AsRef<[u8]>,
    {
        let command = Command::new("LPOP").arg(&list);

        Ok(self.run_command(command).await?.optional_string())
    }

    ///Pop a value from a list from the right side.
    ///# Return value
    ///The value popped from `list`
    pub async fn rpop<K>(&mut self, list: K) -> Result<Option<Vec<u8>>>
    where
        K: AsRef<[u8]>,
    {
        let command = Command::new("RPOP").arg(&list);

        Ok(self.run_command(command).await?.optional_string())
    }

    ///Pop a value from one of the lists from the left side.
    ///Block timeout seconds when there are no values to pop. A zero-value with block infinitely.
    ///# Return value
    ///* `Ok(Some((list,value)))`: name of the list and corresponding value
    ///* `Ok(None)`: timeout (no values)
    ///* `Err(err)`: there was an error
    pub async fn blpop<K>(
        &mut self,
        lists: &[K],
        timeout: u32,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>>
    where
        K: AsRef<[u8]>,
    {
        self.blpop_brpop(lists, timeout, "BLPOP").await
    }

    ///Pop a value from one of the lists from the right side.
    ///Block timeout seconds when there are no values to pop. A zero-value will block infinitely.
    ///# Return value
    ///* `Ok(Some((list,value)))`: name of the list and corresponding value
    ///* `Ok(None)`: timeout (no values)
    ///* `Err(err)`: there was an error
    pub async fn brpop<K>(
        &mut self,
        lists: &[K],
        timeout: u32,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>>
    where
        K: AsRef<[u8]>,
    {
        self.blpop_brpop(lists, timeout, "BRPOP").await
    }

    ///blpop and brpop common code
    async fn blpop_brpop<K>(
        &mut self,
        lists: &[K],
        timeout: u32,
        redis_cmd: &str,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>>
    where
        K: AsRef<[u8]>,
    {
        let timeout = timeout.to_string();
        let command = Command::new(redis_cmd).args(&lists).arg(&timeout);
        match self.run_command(command).await? {
            Value::Array(values) => {
                let vlen = values.len();
                if vlen == 2 {
                    let mut v = values.into_iter().map(|s| s.unwrap_string());
                    return Ok(Some(
                        (v.next().unwrap(), v.next().unwrap()), // values.into_iter().map(|s| s.unwrap_string()).collect(),
                    ));
                }
                Err(Error::UnexpectedResponse(format!(
                    "{}: wrong number of elements received: {}",
                    redis_cmd, vlen
                )))
            }
            Value::Nil => Ok(None),
            other => Err(Error::UnexpectedResponse(format!(
                "{}: {:?}",
                redis_cmd, other
            ))),
        }
    }

    ///Get a series of elements from `list`, from index `from` to `to`. If they are negative, take the
    ///index from the right side of the list.
    pub async fn lrange<K>(&mut self, list: K, from: isize, to: isize) -> Result<Vec<Vec<u8>>>
    where
        K: AsRef<[u8]>,
    {
        let from = from.to_string();
        let to = to.to_string();
        let command = Command::new("LRANGE").arg(&list).arg(&from).arg(&to);

        Ok(self
            .run_command(command)
            .await?
            .unwrap_array()
            .into_iter()
            .map(|s| s.unwrap_string())
            .collect())
    }

    ///Get the number of elements in `list`, or `None` if the list doesn't exist.
    pub async fn llen<K>(&mut self, list: K) -> Result<Option<isize>>
    where
        K: AsRef<[u8]>,
    {
        let command = Command::new("LLEN").arg(&list);
        Ok(self.run_command(command).await?.optional_integer())
    }

    ///Set the value of the element at `index` in `list` to `value`.
    pub async fn lset<K, V>(&mut self, list: K, index: usize, value: V) -> Result<()>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        let index = index.to_string();
        let command = Command::new("LSET").arg(&list).arg(&index).arg(&value);

        self.run_command(command).await?;
        Ok(())
    }

    ///Trim `list` from `start` to `stop`.
    pub async fn ltrim<K>(&mut self, list: K, start: usize, stop: usize) -> Result<()>
    where
        K: AsRef<[u8]>,
    {
        let start = start.to_string();
        let stop = stop.to_string();
        let command = Command::new("LTRIM").arg(&list).arg(&start).arg(&stop);
        self.run_command(command).await?;

        Ok(())
    }

    ///Increment `key` by one.
    ///# Return value
    ///The new value of `key`.
    pub async fn incr<K>(&mut self, key: K) -> Result<isize>
    where
        K: AsRef<[u8]>,
    {
        let command = Command::new("INCR").arg(&key);
        Ok(self.run_command(command).await?.unwrap_integer())
    }

    ///Increment `key` by `val`.
    ///# Return value
    ///The new value of `key`
    pub async fn incrby<K>(&mut self, key: K, val: isize) -> Result<isize>
    where
        K: AsRef<[u8]>,
    {
        let val = val.to_string();
        let command = Command::new("INCRBY").arg(&key).arg(&val);
        Ok(self.run_command(command).await?.unwrap_integer())
    }

    ///Increment `key` by a floating point value `val`.
    ///# Return value
    ///The new value of `key`
    pub async fn incrbyfloat<K>(&mut self, key: K, val: f64) -> Result<f64>
    where
        K: AsRef<[u8]>,
    {
        let val = val.to_string();
        let command = Command::new("INCRBYFLOAT").arg(&key).arg(&val);
        let result = self.run_command(command).await?.unwrap_string();
        Ok(String::from_utf8_lossy(&result).parse::<f64>().unwrap())
    }

    ///Decrement `key` by a floating point value `val`.
    ///# Return value
    ///The new value of `key`
    pub async fn decr<K>(&mut self, key: K) -> Result<isize>
    where
        K: AsRef<[u8]>,
    {
        let command = Command::new("DECR").arg(&key);
        Ok(self.run_command(command).await?.unwrap_integer())
    }

    ///Decrement `key` by `val`.
    ///# Return value
    ///The new value of `key`
    pub async fn decrby<K>(&mut self, key: K, val: isize) -> Result<isize>
    where
        K: AsRef<[u8]>,
    {
        let val = val.to_string();
        let command = Command::new("DECRBY").arg(&key).arg(&val);
        Ok(self.run_command(command).await?.unwrap_integer())
    }

    ///Append a string `val` to `key`.
    ///# Return value
    ///The new size of `key`
    pub async fn append<K, V>(&mut self, key: K, val: V) -> Result<isize>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        let command = Command::new("APPEND").arg(&key).arg(&val);
        Ok(self.run_command(command).await?.unwrap_integer())
    }

    ///Get the string value for every `key`, or `None`` if it doesn't exist
    pub async fn mget<K>(&mut self, keys: &[K]) -> Result<Vec<Option<Vec<u8>>>>
    where
        K: AsRef<[u8]>,
    {
        let command = Command::new("MGET").args(&keys);
        let result = self.run_command(command).await?.unwrap_array();
        let output: Vec<Option<Vec<u8>>> =
            result.into_iter().map(|r| r.optional_string()).collect();
        Ok(output)
    }

    ///Set every key in `builder` to their respective values.
    ///# Example
    ///```
    /// use darkredis::{Connection, MSetBuilder};
    ///# #[cfg_attr(feature = "runtime_tokio", tokio::main)]
    ///# #[cfg_attr(feature = "runtime_async_std", async_std::main)]
    /// # async fn main() {
    /// let mut connection = Connection::connect("127.0.0.1:6379").await.unwrap();
    /// let builder = MSetBuilder::new()
    ///     .set(b"multi-key-1", b"foo")
    ///     .set(b"multi-key-2", b"bar")
    ///     .set(b"multi-key-3", b"baz");
    /// connection.mset(builder).await.unwrap();
    /// let keys = &[b"multi-key-1", b"multi-key-2", b"multi-key-3"];
    /// let results = connection.mget(keys).await.unwrap();
    /// assert_eq!(
    ///     results,
    ///     vec![
    ///         Some(b"foo".to_vec()),
    ///         Some(b"bar".to_vec()),
    ///         Some(b"baz".to_vec())
    ///     ]
    /// );
    /// # connection.del_slice(keys).await.unwrap();
    /// # }
    ///```

    pub async fn mset(&mut self, builder: MSetBuilder<'_>) -> Result<()>
where {
        let mut command = Command::new("MSET");
        command.append_msetbuilder(&builder);
        self.run_command(command).await?;
        Ok(())
    }

    ///Returns true if a key has been previously set.
    pub async fn exists<K>(&mut self, key: K) -> Result<bool>
    where
        K: AsRef<[u8]>,
    {
        let command = Command::new("EXISTS").arg(&key);
        Ok(self.run_command(command).await? == Value::Integer(1))
    }

    ///Adds new `value` to set specified by `key`.
    pub async fn sadd<K, V>(&mut self, key: K, value: V) -> Result<bool>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        let command = Command::new("SADD").arg(&key).arg(&value);

        Ok(self.run_command(command).await?.unwrap_bool())
    }

    ///Like [`sadd`](struct.Connection.html#method.sadd), but push multiple values.
    pub async fn sadd_slice<K, V>(&mut self, key: K, values: &[V]) -> Result<isize>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        let command = Command::new("SADD").arg(&key).args(&values);

        Ok(self.run_command(command).await?.unwrap_integer())
    }

    /// Return the members of a set specified by `key`.
    pub async fn smembers<K>(&mut self, key: K) -> Result<Vec<Vec<u8>>>
    where
        K: AsRef<[u8]>,
    {
        let command = Command::new("SMEMBERS").arg(&key);

        Ok(self
            .run_command(command)
            .await?
            .unwrap_array()
            .into_iter()
            .map(|s| s.unwrap_string())
            .collect())
    }

    /// Returns `true` if `value` belongs to a set specified by `key`.
    pub async fn sismember<K, V>(&mut self, key: K, value: V) -> Result<bool>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        let command = Command::new("SISMEMBER").arg(&key).arg(&value);

        Ok(self.run_command(command).await?.unwrap_bool())
    }

    ///Scan for elements in a set.
    ///# Return value
    ///Returns a list of the elements which matched in the set.
    ///# Example
    ///```
    /// use darkredis::Connection;
    /// use futures::StreamExt;
    ///# #[cfg_attr(feature = "runtime_tokio", tokio::main)]
    ///# #[cfg_attr(feature = "runtime_async_std", async_std::main)]
    /// # async fn main() {
    ///
    /// let mut connection = Connection::connect("127.0.0.1:6379").await.unwrap();
    /// let key = b"test-set".to_vec();
    /// # connection.del(&key).await.unwrap();
    /// connection.sadd(&key, "foo").await.unwrap();
    /// connection.sadd(&key, "bar").await.unwrap();
    ///
    /// let results = connection.sscan(&key).run()
    ///   .collect::<Vec<Vec<u8>>>().await;
    ///
    /// assert!(results.contains(&b"foo".to_vec()));
    /// assert!(results.contains(&b"bar".to_vec()));
    /// # connection.del(&key).await.unwrap();
    /// # }
    ///```
    pub fn sscan<'a, K>(&'a mut self, key: &'a K) -> ScanBuilder
    where
        K: AsRef<[u8]>,
    {
        ScanBuilder::new("SSCAN", Some(key.as_ref()), self)
    }

    ///Scan for keys in the database.
    ///# Return Value
    /// A stream of the matching keys.
    ///# Example
    ///```no_run
    /// use darkredis::Connection;
    /// use futures::StreamExt;
    ///# #[cfg_attr(feature = "runtime_tokio", tokio::main)]
    ///# #[cfg_attr(feature = "runtime_async_std", async_std::main)]
    /// # async fn main() {
    ///
    /// let mut connection = Connection::connect("127.0.0.1:6379").await.unwrap();
    /// let key = b"locate-me".to_vec();
    /// connection.set(&key, "dummy-value").await.unwrap();
    /// let results = connection.scan().pattern(b"locate*").run()
    ///   .collect::<Vec<Vec<u8>>>().await;
    ///
    /// assert!(results.contains(&key));
    /// # connection.del(&key).await.unwrap();
    /// # }
    ///```
    pub fn scan(&mut self) -> ScanBuilder {
        ScanBuilder::new("SCAN", None, self)
    }

    ///Scan for fields in the hash set at `key`.
    ///# Example
    ///```
    /// use darkredis::Connection;
    /// use futures::StreamExt;
    ///# #[cfg_attr(feature = "runtime_tokio", tokio::main)]
    ///# #[cfg_attr(feature = "runtime_async_std", async_std::main)]
    /// # async fn main() {
    ///
    /// let mut connection = Connection::connect("127.0.0.1:6379").await.unwrap();
    /// let key = b"hscan_test".to_vec();
    /// connection.hset(&key, "one", "1").await.unwrap();
    /// connection.hset(&key, "two", "2").await.unwrap();
    /// connection.hset(&key, "three", "3").await.unwrap();
    /// let results = connection.hscan(&key).run()
    ///   .collect::<Vec<(Vec<u8>, Vec<u8>)>>().await;
    ///
    /// assert_eq!(results.len(), 3);
    /// assert!(results.contains(&(b"one".to_vec(), b"1".to_vec())));
    /// assert!(results.contains(&(b"two".to_vec(), b"2".to_vec())));
    /// assert!(results.contains(&(b"three".to_vec(), b"3".to_vec())));
    /// # connection.del(&key).await.unwrap();
    /// # }
    pub fn hscan<'a, K>(&'a mut self, key: &'a K) -> HScanBuilder<'a>
    where
        K: AsRef<[u8]>,
    {
        HScanBuilder::new(key.as_ref(), self)
    }

    ///Get the Type of `key` using the `TYPE` command.
    pub async fn key_type<K>(&mut self, key: K) -> Result<Option<DataType>>
    where
        K: AsRef<[u8]>,
    {
        let command = Command::new("TYPE").arg(&key);
        let result = self.run_command(command).await?.unwrap_string();
        match result.as_slice() {
            b"string\r\n" => Ok(Some(DataType::String)),
            b"list\r\n" => Ok(Some(DataType::List)),
            b"set\r\n" => Ok(Some(DataType::Set)),
            b"zset\r\n" => Ok(Some(DataType::ZSet)),
            b"hash\r\n" => Ok(Some(DataType::Hash)),
            b"stream\r\n" => Ok(Some(DataType::Stream)),
            b"none\r\n" => Ok(None),
            _ => Err(Error::UnexpectedResponse(
                String::from_utf8_lossy(&result).to_string(),
            )),
        }
    }

    ///Get the number of members in the set at `key`.
    pub async fn scard<K>(&mut self, key: &K) -> Result<isize>
    where
        K: AsRef<[u8]>,
    {
        let command = Command::new("SCARD").arg(&key);
        Ok(self.run_command(command).await?.unwrap_integer())
    }

    ///Move set member `member` from `source` to `destination`.
    ///# Return value
    ///`true` if the member was moved.
    pub async fn smove<S, D, M>(&mut self, source: S, destination: D, member: M) -> Result<bool>
    where
        S: AsRef<[u8]>,
        M: AsRef<[u8]>,
        D: AsRef<[u8]>,
    {
        let command = Command::new("SMOVE")
            .arg(&source)
            .arg(&destination)
            .arg(&member);
        Ok(self.run_command(command).await?.unwrap_bool())
    }

    ///Remove set member `member`, from the set `key`.
    ///# Return value
    ///`true` if the member was removed.
    pub async fn srem<K, M>(&mut self, key: K, member: M) -> Result<bool>
    where
        K: AsRef<[u8]>,
        M: AsRef<[u8]>,
    {
        let command = Command::new("SREM").arg(&key).arg(&member);
        Ok(self.run_command(command).await?.unwrap_bool())
    }

    ///Remove every member in `members` from the set at `key`.
    ///# Return value
    ///The number of members which were removed.
    pub async fn srem_slice<K, M>(&mut self, key: K, members: &[M]) -> Result<isize>
    where
        K: AsRef<[u8]>,
        M: AsRef<[u8]>,
    {
        check_slice_not_empty!(members);
        let command = Command::new("SREM").arg(&key).args(members);
        Ok(self.run_command(command).await?.unwrap_integer())
    }

    ///Return the difference in members between the first set and all the other sets.
    pub async fn sdiff<S>(&mut self, sets: &[S]) -> Result<Vec<Vec<u8>>>
    where
        S: AsRef<[u8]>,
    {
        check_slice_not_empty!(sets);
        let command = Command::new("SDIFF").args(sets);
        Ok(self.run_command(command).await?.unwrap_string_array())
    }

    ///Place the difference in members between the sets into `destination`.
    ///# Return value
    ///The number of elements in `destination` after the operation.
    pub async fn sdiffstore<D, S>(&mut self, destination: D, sets: &[S]) -> Result<isize>
    where
        D: AsRef<[u8]>,
        S: AsRef<[u8]>,
    {
        check_slice_not_empty!(sets);
        let command = Command::new("SDIFFSTORE").arg(&destination).args(sets);
        Ok(self.run_command(command).await?.unwrap_integer())
    }

    ///Return the members which are in every set.
    pub async fn sinter<S>(&mut self, sets: &[S]) -> Result<Vec<Vec<u8>>>
    where
        S: AsRef<[u8]>,
    {
        check_slice_not_empty!(sets);
        let command = Command::new("SINTER").args(sets);
        Ok(self.run_command(command).await?.unwrap_string_array())
    }

    ///Create a new set at `destination` containing the members which are part of all sets.
    ///# Return value
    ///The number of elements in `destination` after the operation.
    pub async fn sinterstore<D, S>(&mut self, destination: D, sets: &[S]) -> Result<isize>
    where
        D: AsRef<[u8]>,
        S: AsRef<[u8]>,
    {
        check_slice_not_empty!(sets);
        let command = Command::new("SINTERSTORE").arg(&destination).args(sets);
        Ok(self.run_command(command).await?.unwrap_integer())
    }

    ///Return a `count` random members of `set`. If `count` is negative, the same element can show up
    ///multiple times. See the [Redis documentation](https://redis.io/commands/srandmember) for more info.
    pub async fn srandmember<S>(&mut self, set: S, count: isize) -> Result<Vec<Vec<u8>>>
    where
        S: AsRef<[u8]>,
    {
        let count = count.to_string();
        let command = Command::new("SRANDMEMBER").arg(&set).arg(&count);
        Ok(self.run_command(command).await?.unwrap_string_array())
    }

    ///Pop `count` random elements out of `set`.
    pub async fn spop<S>(&mut self, set: S, count: isize) -> Result<Vec<Vec<u8>>>
    where
        S: AsRef<[u8]>,
    {
        let count = count.to_string();
        let command = Command::new("SPOP").arg(&set).arg(&count);
        Ok(self.run_command(command).await?.unwrap_string_array())
    }

    ///Return the union of every set in `sets`.
    pub async fn sunion<S>(&mut self, sets: &[S]) -> Result<Vec<Vec<u8>>>
    where
        S: AsRef<[u8]>,
    {
        check_slice_not_empty!(sets);
        let command = Command::new("SUNION").args(sets);
        Ok(self.run_command(command).await?.unwrap_string_array())
    }

    ///Store the union of `sets` in `destination`.
    ///# Return value
    ///The number of elements in `destination` after the operation.
    pub async fn sunionstore<D, S>(&mut self, destination: D, sets: &[S]) -> Result<isize>
    where
        D: AsRef<[u8]>,
        S: AsRef<[u8]>,
    {
        check_slice_not_empty!(sets);
        let command = Command::new("SUNIONSTORE").arg(&destination).args(sets);
        Ok(self.run_command(command).await?.unwrap_integer())
    }
}
